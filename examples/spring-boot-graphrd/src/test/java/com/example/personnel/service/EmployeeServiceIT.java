package com.example.personnel.service;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.NoSuchElementException;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.test.context.TestConstructor;
import org.springframework.test.context.TestConstructor.AutowireMode;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.data.domain.PageRequest;
import org.springframework.data.domain.Sort;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;

import com.example.personnel.TestServer;
import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.dto.EmployeeIn;
import com.example.personnel.model.dto.EmployeeOut;
import com.example.personnel.model.dto.RdfRepresentation;
import com.example.personnel.model.filter.EmployeeSearch;
import com.graphrd.RdfFormat;

/** The employee logic against a live GraphRD: validation, raises, history, search, RDF. */
@TestConstructor(autowireMode = AutowireMode.ALL)
@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.NONE)
class EmployeeServiceIT {

    private final EmployeeService employees;
    private final CompanyService companies;

    EmployeeServiceIT(EmployeeService employees, CompanyService companies) {
        this.employees = employees;
        this.companies = companies;
    }

    @BeforeAll
    static void up() throws Exception {
        System.setProperty("graphrd.test.url", TestServer.start());
    }

    @DynamicPropertySource
    static void pointing(DynamicPropertyRegistry registry) {
        registry.add("graphrd.base-url", () -> System.getProperty("graphrd.test.url"));
        registry.add("graphrd.repository", () -> "svc-it");
    }

    private String acme;
    private String aliceId;

    @AfterAll
    static void down() {
        TestServer.stop();
    }

    @BeforeEach
    void fresh() {
        employees.repository().deleteAll();
        companies.repository().deleteAll();
        acme = companies.create(new CompanyIn("Acme", null)).getId();
        String initech = companies.create(new CompanyIn("Initech", null)).getId();
        aliceId = employees.create(new EmployeeIn("Alice", "alice@example.org", 34, 82000, "2021-03-15", acme)).id();
        employees.create(new EmployeeIn("Bob", "bob@example.org", 28, 61000, "2023-07-01", acme));
        employees.create(new EmployeeIn("Carol", "carol@example.org", 41, 95000, "2019-11-20", initech));
    }

    @Test
    void duplicateEmailIsRejected() {
        var ex = assertThrows(IllegalArgumentException.class,
                () -> employees.create(new EmployeeIn("A2", "alice@example.org", 30, 1, null, acme)));
        assertTrue(ex.getMessage().contains("already in use"));
    }

    @Test
    void unknownCompanyIsRejected() {
        assertThrows(IllegalArgumentException.class,
                () -> employees.create(new EmployeeIn("X", "x@example.org", 30, 1, null, "http://nope")));
    }

    @Test
    void createMintsTheIriAndJoinsTheCompany() {
        EmployeeOut created = employees.create(new EmployeeIn("Dave", "dave@example.org", 52, 58000, null, acme));
        assertNotNull(created.id());
        assertTrue(created.id().contains("/employee/"), created.id());
        assertEquals("Acme", created.companyName());
        assertEquals("Acme", employees.get(created.id()).companyName());
    }

    @Test
    void raiseRulesAndTheRecord() {
        assertThrows(IllegalArgumentException.class, () -> employees.raise(aliceId, 0, null));
        assertThrows(IllegalArgumentException.class, () -> employees.raise(aliceId, 999999, null));
        EmployeeOut raised = employees.raise(aliceId, 4000, "merit");
        assertEquals(86000, raised.salary());
        var history = employees.history(aliceId);
        assertEquals(1, history.size());
        assertEquals(4000, history.get(0).amount());
        assertEquals("merit", history.get(0).reason());
    }

    @Test
    void searchPagesSortsAndFilters() {
        var page = employees.search(new EmployeeSearch(null, null, null, null),
                PageRequest.of(0, 2, Sort.by("salary").descending()));
        assertEquals(3, page.getTotalElements());
        assertEquals(2, page.getContent().size());
        assertEquals("Carol", page.getContent().get(0).name());

        var atAcme = employees.search(new EmployeeSearch(30, null, null, acme), PageRequest.of(0, 20));
        assertEquals(1, atAcme.getTotalElements());
        assertEquals("Alice", atAcme.getContent().get(0).name());

        var byName = employees.search(new EmployeeSearch(null, null, "aL", null), PageRequest.of(0, 20));
        assertEquals(1, byName.getTotalElements());
    }

    @Test
    void rdfRepresentationsFollowAccept() {
        RdfRepresentation turtle = employees.oneAsRdf(aliceId, "text/turtle");
        assertEquals("text/turtle", turtle.mediaType());
        assertTrue(turtle.body().contains("salary"), turtle.body());

        RdfRepresentation jsonld = employees.oneAsRdf(aliceId, null); // unknown Accept → JSON-LD
        assertEquals(RdfFormat.JSONLD.mediaType, jsonld.mediaType());
        assertTrue(jsonld.body().contains("\"@id\":\"" + aliceId + "\""), jsonld.body());

        RdfRepresentation all = employees.listAsRdf("text/turtle");
        assertTrue(all.body().split("a ex:Employee", -1).length - 1 >= 3, all.body());
    }

    @Test
    void deleteRemovesTheEmployee() {
        EmployeeOut temp = employees.create(new EmployeeIn("Temp", "temp@example.org", 30, 1, null, acme));
        employees.repository().deleteById(temp.id());
        assertThrows(NoSuchElementException.class, () -> employees.get(temp.id()));
    }
}
