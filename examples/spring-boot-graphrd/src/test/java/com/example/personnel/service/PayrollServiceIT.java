package com.example.personnel.service;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.test.context.TestConstructor;
import org.springframework.test.context.TestConstructor.AutowireMode;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;

import com.example.personnel.TestServer;
import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.dto.EmployeeIn;
import com.example.personnel.model.dto.PayrollRow;

/** The reports against a live GraphRD: @Query aggregates that must agree with each other. */
@TestConstructor(autowireMode = AutowireMode.ALL)
@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.NONE)
class PayrollServiceIT {

    private final EmployeeService employees;
    private final CompanyService companies;
    private final PayrollService payroll;
    private final com.example.personnel.repository.ReviewsRepository reviews;

    PayrollServiceIT(EmployeeService employees, CompanyService companies, PayrollService payroll, com.example.personnel.repository.ReviewsRepository reviews) {
        this.employees = employees;
        this.companies = companies;
        this.payroll = payroll;
        this.reviews = reviews;
    }

    @BeforeAll
    static void up() throws Exception {
        System.setProperty("graphrd.test.url", TestServer.start());
    }

    @DynamicPropertySource
    static void pointing(DynamicPropertyRegistry registry) {
        registry.add("graphrd.base-url", () -> System.getProperty("graphrd.test.url"));
        registry.add("graphrd.repository", () -> "pay-it");
    }

    private String acme;

    @AfterAll
    static void down() {
        TestServer.stop();
    }

    @BeforeEach
    void fresh() {
        reviews.deleteAll();
        employees.repository().deleteAll();
        companies.repository().deleteAll();
        acme = companies.create(new CompanyIn("Acme", null)).getId();
        employees.create(new EmployeeIn("Alice", "a@x.org", 34, 82000, null, acme));
        employees.create(new EmployeeIn("Bob", "b@x.org", 28, 61000, null, acme));
    }

    @Test
    void payrollRowsMatchTheTotals() {
        var rows = payroll.payroll();
        assertEquals(1, rows.size());
        PayrollRow row = rows.get(0);
        assertEquals("Acme", row.company());
        assertEquals(2, row.heads());
        assertEquals(143000, row.total());
        assertEquals(71500.0, row.average());
        assertEquals(61000, row.lowest());
        assertEquals(82000, row.highest());

        var totals = payroll.totals();
        assertEquals(2, totals.employees());
        assertEquals(143000, totals.payroll());
        assertEquals(71500.0, totals.averageSalary());
        long sum = rows.stream().mapToLong(PayrollRow::total).sum();
        assertEquals(totals.payroll(), sum, "rows sum to the totals");
    }

    @Test
    void parameterizedRowAndCostSince() {
        var alice = employees.pageAll().stream()
                .filter(e -> e.name().equals("Alice")).findFirst().orElseThrow();
        employees.raise(alice.id(), 4000, "merit");

        PayrollRow row = payroll.companyPayroll("Acme");
        assertEquals(86000, row.highest()); // 82000 + 4000

        assertEquals(4000, payroll.raiseCost());
        assertEquals(0, payroll.raiseCostSince("2999-01-01"));
        assertTrue(payroll.raiseCostSince("2000-01-01") >= 4000);
    }
}
