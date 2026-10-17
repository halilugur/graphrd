package com.example.personnel.service;

import java.util.List;

import org.springframework.stereotype.Service;

import com.example.personnel.model.dto.PayrollRow;
import com.example.personnel.model.dto.PayrollStats;
import com.example.personnel.model.dto.Totals;
import com.example.personnel.repository.CompaniesRepository;

import com.example.personnel.repository.EmployeeRepository;
import com.example.personnel.repository.ReviewsRepository;
import com.graphrd.spring.GraphRdProperties;
import com.graphrd.RdfClient;

import lombok.RequiredArgsConstructor;

/**
 * The numbers, all declared as @Query aggregates on the one
 * {@link EmployeeRepository} and computed by the server in SPARQL — this
 * service only puts the pieces together.
 */
@Service
@RequiredArgsConstructor
public class PayrollService {

    private final EmployeeRepository employees; // the one repository carries the reports too
    private final CompaniesRepository companies;
    private final ReviewsRepository reviews;

    /** Per company: headcount, payroll, average, spread. */
    public List<PayrollRow> payroll() {
        return employees.payroll();
    }

    /** The whole organization in one row. */
    public Totals totals() {
        PayrollStats stats = employees.payrollStats();
        return new Totals(stats.employees(), companies.count(),
                stats.payroll(), stats.average(), reviews.count());
    }

    /** What raises have cost so far. */
    public long raiseCost() {
        return employees.raiseCost();
    }

    /** One company's row, by its name (a parameterized @Query). */
    public PayrollRow companyPayroll(String name) {
        return employees.companyPayroll(name);
    }

    /** The raise cost since a date (a parameterized @Query). */
    public long raiseCostSince(String since) {
        return employees.raiseCostSince(since);
    }
}
