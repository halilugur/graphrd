package com.example.personnel.model.entity;

import org.graphrd.rdf.RdfId;
import org.graphrd.rdf.RdfProperty;
import org.graphrd.rdf.RdfType;

import lombok.AllArgsConstructor;
import lombok.Getter;
import lombok.NoArgsConstructor;

/**
 * One salary review on record: written when a raise happens, kept forever.
 * The {@code employee} reference makes the history a join, not a copy.
 */
@Getter
@NoArgsConstructor
@AllArgsConstructor
@RdfType("SalaryReview")
public class SalaryReview {

    @RdfId(generated = true)
    String id;

    @RdfProperty("employee")
    Employee employee;

    @RdfProperty("date")
    String date;

    @RdfProperty("oldSalary")
    Integer oldSalary;

    @RdfProperty("newSalary")
    Integer newSalary;

    @RdfProperty("reason")
    String reason;
}
